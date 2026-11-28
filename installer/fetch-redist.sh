#!/bin/sh
# Downloads cloudflared for the Linux release tarballs into installer/redist, checking each file
# against its pinned SHA-256. Move to a new version by changing the version and the hashes together.

set -eu

VERSION=2026.10.0
dir=$(dirname "$0")/redist
mkdir -p "$dir"

for pair in \
    amd64:d33ff2d14475178d2012c2c56beba87389ac5ded27649519f198a7d3134a99db \
    arm64:e6422b9d4f72d3194bc5a38676f13667c06666523217b842a877d72a80b5ac08; do
    arch=${pair%%:*}
    want=${pair#*:}
    file="$dir/cloudflared-linux-$arch"
    if [ ! -f "$file" ] || [ "$(sha256sum "$file" | cut -d' ' -f1)" != "$want" ]; then
        echo "Downloading cloudflared-linux-$arch"
        curl -fsSL --retry 3 -o "$file" \
            "https://github.com/cloudflare/cloudflared/releases/download/$VERSION/cloudflared-linux-$arch"
    fi
    got=$(sha256sum "$file" | cut -d' ' -f1)
    if [ "$got" != "$want" ]; then
        rm -f "$file"
        echo "cloudflared-linux-$arch: SHA-256 is $got, expected $want" >&2
        exit 1
    fi
    echo "cloudflared-linux-$arch OK"
done
