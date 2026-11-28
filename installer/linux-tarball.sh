#!/bin/sh
# Packs the Linux host into the release tarball web-stream-host-VERSION-linux-ARCH.tar.gz.
# usage: installer/linux-tarball.sh VERSION ARCH WEB_STREAM_BINARY [CLOUDFLARED] OUT_DIR

set -eu

if [ $# -lt 4 ]; then
    echo "usage: $0 VERSION ARCH WEB_STREAM_BINARY [CLOUDFLARED] OUT_DIR" >&2
    exit 2
fi
version=$1
arch=$2
binary=$3
if [ $# -ge 5 ]; then
    cloudflared=$4
    out=$5
else
    cloudflared=""
    out=$4
fi
here=$(cd "$(dirname "$0")" && pwd)
name="web-stream-host-$version-linux-$arch"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/$name/third-party" "$out"
install -m 0755 "$binary" "$stage/$name/web-stream"
if [ -n "$cloudflared" ]; then
    install -m 0755 "$cloudflared" "$stage/$name/cloudflared"
    cp "$here/third-party/cloudflared-LICENSE.txt" "$stage/$name/third-party/"
fi
tar -C "$stage" --owner=0 --group=0 --numeric-owner -czf "$out/$name.tar.gz" "$name"
echo "$out/$name.tar.gz"
