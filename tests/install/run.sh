#!/bin/sh

set -eu

workspace=$(mktemp -d)
trap 'rm -rf "$workspace"' EXIT HUP INT TERM

make_release() {
    release_dir=$1
    package_dir="${workspace}/package"
    rm -rf "$package_dir"
    mkdir -p "$package_dir" "$release_dir"
    cat > "${package_dir}/auto-airplay" <<'EOF'
#!/bin/sh
printf '%s\n' 'auto-airplay test binary'
EOF
    chmod 0755 "${package_dir}/auto-airplay"
    cp packaging/auto-airplay@.service "${package_dir}/auto-airplay@.service"
    printf '%s\n' 'test readme' > "${package_dir}/README.md"
    printf '%s\n' 'test license' > "${package_dir}/LICENSE"
    tar -C "$package_dir" -czf \
        "${release_dir}/auto-airplay-aarch64-unknown-linux-gnu.tar.gz" .
    cd "$release_dir"
    sha256sum auto-airplay-aarch64-unknown-linux-gnu.tar.gz > \
        auto-airplay-aarch64-unknown-linux-gnu.tar.gz.sha256
    cd /source
}

latest_dir="${workspace}/releases/latest/download"
make_release "$latest_dir"

root="${workspace}/root"
mkdir -p "${root}/etc/auto-airplay"
printf '%s\n' 'keep this configuration' > "${root}/etc/auto-airplay/config.toml"
AUTO_AIRPLAY_ARCH=aarch64 \
AUTO_AIRPLAY_ALLOW_FILE_URLS=1 \
AUTO_AIRPLAY_RELEASE_BASE_URL="file://${workspace}/releases" \
    ./install.sh \
        --user tester \
        --prefix "${root}/usr/local" \
        --config-dir "${root}/etc/auto-airplay" \
        --systemd-dir "${root}/etc/systemd/system"

test -x "${root}/usr/local/bin/auto-airplay"
test -f "${root}/etc/systemd/system/auto-airplay@.service"
test "$(stat -c %a "${root}/usr/local/bin/auto-airplay")" = "755"
test "$(stat -c %a "${root}/etc/systemd/system/auto-airplay@.service")" = "644"
test "$(stat -c %a "${root}/etc/auto-airplay")" = "750"
test "$(stat -c %U "${root}/etc/auto-airplay")" = "tester"
test "$("${root}/usr/local/bin/auto-airplay")" = "auto-airplay test binary"
test "$(cat "${root}/etc/auto-airplay/config.toml")" = "keep this configuration"

version_dir="${workspace}/releases/download/v0.1.0"
make_release "$version_dir"
AUTO_AIRPLAY_ARCH=arm64 \
AUTO_AIRPLAY_ALLOW_FILE_URLS=1 \
AUTO_AIRPLAY_RELEASE_BASE_URL="file://${workspace}/releases" \
    ./install.sh \
        --user tester \
        --version v0.1.0 \
        --prefix "${workspace}/version-root/usr/local" \
        --config-dir "${workspace}/version-root/etc/auto-airplay" \
        --systemd-dir "${workspace}/version-root/etc/systemd/system"
test -x "${workspace}/version-root/usr/local/bin/auto-airplay"

tampered_dir="${workspace}/tampered/latest/download"
make_release "$tampered_dir"
printf '%064d  %s\n' 0 auto-airplay-aarch64-unknown-linux-gnu.tar.gz > \
    "${tampered_dir}/auto-airplay-aarch64-unknown-linux-gnu.tar.gz.sha256"

if AUTO_AIRPLAY_ARCH=aarch64 \
    AUTO_AIRPLAY_ALLOW_FILE_URLS=1 \
    AUTO_AIRPLAY_RELEASE_BASE_URL="file://${workspace}/tampered" \
    ./install.sh \
        --user tester \
        --prefix "${workspace}/tampered-root/usr/local" \
        --config-dir "${workspace}/tampered-root/etc/auto-airplay" \
        --systemd-dir "${workspace}/tampered-root/etc/systemd/system"; then
    printf '%s\n' 'tampered release unexpectedly installed' >&2
    exit 1
fi
test ! -e "${workspace}/tampered-root/usr/local/bin/auto-airplay"

if AUTO_AIRPLAY_ARCH=aarch64 ./install.sh --user tester --version 'v0.1.0/../../bad'; then
    printf '%s\n' 'unsafe version unexpectedly accepted' >&2
    exit 1
fi

if AUTO_AIRPLAY_ARCH=mips64 ./install.sh --user tester; then
    printf '%s\n' 'unsupported architecture unexpectedly installed' >&2
    exit 1
fi

if su tester -c './install.sh --user tester'; then
    printf '%s\n' 'non-root installation unexpectedly succeeded' >&2
    exit 1
fi

printf '%s\n' 'installer tests passed'
