#!/bin/sh
# Installs or updates the web-stream host on Linux with systemd, in one step:
#   curl -fsSL https://github.com/Gogan999/web-stream/releases/latest/download/web-stream-install.sh | sh
# Uninstalls (keeping /var/lib/web-stream unless --purge):
#   curl -fsSL .../web-stream-install.sh | sh -s -- --uninstall [--purge]
# Environment: WEB_STREAM_RELEASES (a mirror of the releases page), WEB_STREAM_REINSTALL=1 (install
# even when current), WEB_STREAM_SYSTEMCTL (a systemctl stand-in, for tests without systemd).

set -eu

LIB=/usr/local/lib/web-stream
DATA=/var/lib/web-stream
LINK=/usr/local/bin/web-stream
UNIT=/etc/systemd/system/web-stream.service
UDEV=/etc/udev/rules.d/70-web-stream-uinput.rules
MODULES=/etc/modules-load.d/web-stream.conf
NETWORK_USER=web-stream

say() { printf '%s\n' "$*"; }
die() { printf 'web-stream install: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# Runs a command as root, through sudo when needed.
as_root() {
    if [ "$(id -u)" = 0 ]; then
        "$@"
    elif have sudo; then
        sudo "$@"
    else
        die "this needs root: run it as root or install sudo"
    fi
}

systemctl_() {
    if [ -n "${WEB_STREAM_SYSTEMCTL:-}" ]; then
        as_root "$WEB_STREAM_SYSTEMCTL" "$@"
    else
        as_root systemctl "$@"
    fi
}

# Downloads $1 to $2.
fetch() {
    if have curl; then
        curl -fsSL --retry 3 -o "$2" "$1"
    elif have wget; then
        wget -q -O "$2" "$1"
    else
        die "needs curl or wget"
    fi
}

sha256() {
    if have sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    elif have shasum; then
        shasum -a 256 "$1" | cut -d' ' -f1
    elif have openssl; then
        openssl dgst -sha256 -r "$1" | cut -d' ' -f1
    else
        die "needs sha256sum, shasum or openssl"
    fi
}

# The value of "key": "text" in the JSON on stdin.
json_string() {
    tr -d '\n\r\t' | sed -n "s/.*\"$1\" *: *\"\([^\"]*\)\".*/\1/p"
}

# The value of "key": number in the JSON on stdin.
json_number() {
    tr -d '\n\r\t' | sed -n "s/.*\"$1\" *: *\([0-9][0-9]*\).*/\1/p"
}

detect_arch() {
    case "$(uname -m)" in
        x86_64 | amd64) echo x86_64 ;;
        aarch64 | arm64) echo aarch64 ;;
        *) die "no release for $(uname -m): build from source (docs/dev/building.md)" ;;
    esac
}

check_system() {
    [ "$(uname -s)" = Linux ] || die "this installer is for Linux; on Windows use web-stream-install.ps1"
    if [ -z "${WEB_STREAM_SYSTEMCTL:-}" ] && [ ! -d /run/systemd/system ]; then
        die "systemd is not running here: the host runs as a systemd service"
    fi
}

uninstall() {
    purge=$1
    say "Removing the web-stream host..."
    if [ -f "$UNIT" ]; then
        systemctl_ disable --now web-stream.service || true
        as_root rm -f "$UNIT"
        systemctl_ daemon-reload || true
    fi
    as_root rm -f "$UDEV" "$MODULES"
    if have udevadm; then as_root udevadm control --reload-rules || true; fi
    if id "$NETWORK_USER" >/dev/null 2>&1; then as_root userdel "$NETWORK_USER" || true; fi
    if [ -L "$LINK" ]; then as_root rm -f "$LINK"; fi
    as_root rm -rf "$LIB"
    if [ "$purge" = 1 ]; then
        as_root rm -rf "$DATA"
        say "web-stream host removed, with its data."
    else
        say "web-stream host removed. Its certificate, paired devices and settings stay in $DATA (--purge removes them)."
    fi
}

install_or_update() {
    repo=${WEB_STREAM_RELEASES:-https://github.com/Gogan999/web-stream/releases}
    repo=${repo%/}
    arch=$(detect_arch)
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 1' INT TERM

    say "Checking the latest web-stream host release..."
    fetch "$repo/latest/download/manifest.json" "$tmp/manifest.json" || die "could not download the release manifest from $repo"
    product=$(json_string product <"$tmp/manifest.json")
    version=$(json_string version <"$tmp/manifest.json")
    [ "$product" = web-stream-host ] || die "the release manifest is not a web-stream host release"
    printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' || die "the release manifest has no valid version"

    name="web-stream-host-$version-linux-$arch.tar.gz"
    entry=$(tr -d '\n\r\t' <"$tmp/manifest.json" | tr '{' '\n' | tr '}' '\n' | grep -F "\"$name\"" | head -n 1 || true)
    size=$(printf '%s' "$entry" | json_number size)
    want=$(printf '%s' "$entry" | json_string sha256 | tr 'A-F' 'a-f')
    if [ -z "$size" ] || ! printf '%s' "$want" | grep -Eq '^[0-9a-f]{64}$'; then
        die "the release $version lists no valid $name"
    fi

    installed=""
    if [ -x "$LIB/web-stream" ]; then installed=$("$LIB/web-stream" version 2>/dev/null || true); fi
    if [ "$installed" = "$version" ] && [ -f "$UNIT" ] && [ -z "${WEB_STREAM_REINSTALL:-}" ]; then
        say "web-stream host $version is already installed and up to date."
        return 0
    fi

    say "Downloading web-stream host $version ($arch)..."
    fetch "$repo/download/v$version/$name" "$tmp/$name" || die "could not download $name"
    got=$(wc -c <"$tmp/$name" | tr -d ' ')
    [ "$got" = "$size" ] || die "the download is $got bytes, the release says $size"
    [ "$(sha256 "$tmp/$name")" = "$want" ] || die "the download does not match the release SHA-256"

    mkdir "$tmp/x"
    tar -xzf "$tmp/$name" -C "$tmp/x"
    dir="$tmp/x/web-stream-host-$version-linux-$arch"
    [ -f "$dir/web-stream" ] || die "$name has no web-stream program"
    chmod 0755 "$dir/web-stream"
    unpacked=$("$dir/web-stream" version 2>/dev/null) || die "the downloaded host does not run here (it needs glibc 2.35 or newer)"
    [ "$unpacked" = "$version" ] || die "the downloaded host is $unpacked, not $version"

    if [ -n "$installed" ]; then
        say "Updating from $installed..."
    else
        say "Installing..."
    fi
    if [ -f "$UNIT" ]; then systemctl_ stop web-stream.service || true; fi
    as_root install -d -m 0755 "$LIB"
    for f in web-stream cloudflared; do
        if [ -f "$dir/$f" ]; then
            as_root install -m 0755 "$dir/$f" "$LIB/$f.new"
            as_root mv -f "$LIB/$f.new" "$LIB/$f"
        fi
    done
    if [ -d "$dir/third-party" ]; then
        as_root rm -rf "$LIB/third-party"
        as_root cp -R "$dir/third-party" "$LIB/third-party"
    fi
    if [ -n "${WEB_STREAM_SYSTEMCTL:-}" ]; then
        as_root env WEB_STREAM_SYSTEMCTL="$WEB_STREAM_SYSTEMCTL" "$LIB/web-stream" service install
    else
        as_root "$LIB/web-stream" service install
    fi
    say "web-stream host $version is installed and running."
}

main() {
    mode=install
    purge=0
    for arg in "$@"; do
        case "$arg" in
            --uninstall) mode=uninstall ;;
            --purge) purge=1 ;;
            -h | --help)
                say "usage: web-stream-install.sh [--uninstall [--purge]]"
                return 0
                ;;
            *) die "unknown option $arg" ;;
        esac
    done
    [ "$purge" = 0 ] || [ "$mode" = uninstall ] || die "--purge goes with --uninstall"
    check_system
    if [ "$mode" = uninstall ]; then
        uninstall "$purge"
    else
        install_or_update
    fi
}

main "$@"
