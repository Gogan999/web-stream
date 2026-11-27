#!/bin/sh
# CI check of the Linux one-line installer against a local release mirror: installs, updates over a
# running host, no-ops when current, refuses a tampered download, uninstalls and purges.
# usage: installer/test-linux-install.sh TARBALL [--fake-systemd]
# --fake-systemd stands in for systemctl where systemd is not PID 1 (a container).

set -u

tarball=$1
fake=${2:-}
here=$(cd "$(dirname "$0")" && pwd)
script="$here/web-stream-install.sh"
port=8765
failures=0
mirror=$(mktemp -d)
chmod 0755 "$mirror"

as_root() {
    if [ "$(id -u)" = 0 ]; then "$@"; else sudo "$@"; fi
}

ok() {
    what=$1
    shift
    if "$@"; then
        echo "ok   $what"
    else
        echo "FAIL $what"
        failures=$((failures + 1))
    fi
}

not() { ! "$@"; }

says() { printf '%s' "$out" | grep -q "$1"; }

session() {
    curl -ks -o /dev/null -w '%{http_code}' --max-time 5 https://127.0.0.1:8443/api/session
}

wait_host() {
    i=0
    while [ $i -lt 60 ]; do
        [ "$(session)" = 401 ] && return 0
        sleep 1
        i=$((i + 1))
    done
    return 1
}

service_pid() { pgrep -f 'web-stream service run' | head -n 1; }
network_runs_unprivileged() { pgrep -u web-stream -f 'web-stream service network' >/dev/null; }

active() {
    if [ -n "$fake" ]; then
        [ -n "$(service_pid)" ]
    else
        systemctl is-active --quiet web-stream
    fi
}

run_installer() {
    WEB_STREAM_RELEASES="http://127.0.0.1:$port" "$@" 2>&1
}

name=$(basename "$tarball")
version=$(printf '%s' "$name" | sed -n 's/^web-stream-host-\(.*\)-linux-[a-z0-9_]*\.tar\.gz$/\1/p')
[ -n "$version" ] || { echo "not a release tarball: $name" >&2; exit 2; }

mkdir -p "$mirror/latest/download" "$mirror/download/v$version"
cp "$tarball" "$mirror/download/v$version/$name"
write_manifest() {
    cat >"$mirror/latest/download/manifest.json" <<EOF
{
  "product": "web-stream-host",
  "version": "$version",
  "notes": "CI {\"version\": \"9.9.9\"}",
  "files": [
    {
      "name": "web-stream-setup-$version.exe",
      "size": 1,
      "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
    },
    {
      "name": "$name",
      "size": $(wc -c <"$tarball" | tr -d ' '),
      "sha256": "$1"
    }
  ]
}
EOF
}
write_manifest "$(sha256sum "$tarball" | cut -d' ' -f1)"

if [ -n "$fake" ]; then
    fakectl="$mirror/systemctl"
    cat >"$fakectl" <<'EOF'
#!/bin/sh
# Stands in for systemctl: runs the service in the background.
pidfile=/run/web-stream-fake.pid
stop() {
    if [ -f $pidfile ]; then
        pid=$(cat $pidfile)
        kill -TERM "$pid" 2>/dev/null
        i=0
        while kill -0 "$pid" 2>/dev/null && [ $i -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
        rm -f $pidfile
    fi
}
start() {
    setsid /usr/local/lib/web-stream/web-stream service run >>/var/log/web-stream-fake.log 2>&1 &
    echo $! >$pidfile
}
case "$1" in
    start | restart) stop; start ;;
    stop) stop ;;
    disable) [ "${2:-}" = --now ] && stop ;;
esac
exit 0
EOF
    chmod 0755 "$fakectl"
    WEB_STREAM_SYSTEMCTL=$fakectl
    export WEB_STREAM_SYSTEMCTL
fi

python3 -m http.server "$port" --bind 127.0.0.1 --directory "$mirror" >/dev/null 2>&1 &
server=$!
trap 'kill $server 2>/dev/null; rm -rf "$mirror"' EXIT
i=0
until curl -fs -o /dev/null "http://127.0.0.1:$port/latest/download/manifest.json" || [ $i -ge 30 ]; do
    sleep 1
    i=$((i + 1))
done

echo "== fresh install (piped into sh)"
out=$(run_installer sh <"$script")
echo "$out"
ok "installer succeeded" test -x /usr/local/lib/web-stream/web-stream
ok "installed version is $version" test "$(/usr/local/lib/web-stream/web-stream version)" = "$version"
ok "cloudflared installed" test -x /usr/local/lib/web-stream/cloudflared
ok "web-stream on PATH" test -L /usr/local/bin/web-stream
ok "web-stream user exists" id web-stream
ok "udev rule installed" test -f /etc/udev/rules.d/70-web-stream-uinput.rules
ok "unit installed" test -f /etc/systemd/system/web-stream.service
ok "service active" active
ok "network side answers /api/session with 401" wait_host
ok "network side runs as web-stream" network_runs_unprivileged
ok "network data is the web-stream user's" test "$(as_root stat -c %U /var/lib/web-stream/network)" = web-stream
ok "settings key is private" test "$(as_root stat -c %a /var/lib/web-stream/network/admin-token)" = 600
first=$(service_pid)

echo "== again: no-op when current (dash)"
shell="sh"
command -v dash >/dev/null && shell=dash
out=$(run_installer "$shell" "$script")
echo "$out"
ok "says up to date" says "up to date"
ok "host not restarted" test "$(service_pid)" = "$first"

echo "== update in place over the running host"
out=$(WEB_STREAM_REINSTALL=1 run_installer sh "$script")
echo "$out"
ok "update succeeded" says "is installed and running"
ok "answers again after the update" wait_host
ok "host restarted" not test "$(service_pid)" = "$first"

echo "== tampered download is refused"
write_manifest 1111111111111111111111111111111111111111111111111111111111111111
before=$(sha256sum /usr/local/lib/web-stream/web-stream)
out=$(WEB_STREAM_REINSTALL=1 run_installer sh "$script")
code=$?
echo "$out"
ok "installer failed" not test "$code" = 0
ok "says why" says "does not match"
ok "installed host untouched" test "$(sha256sum /usr/local/lib/web-stream/web-stream)" = "$before"
ok "still answering" wait_host
write_manifest "$(sha256sum "$tarball" | cut -d' ' -f1)"

echo "== uninstall keeps the data"
out=$(run_installer sh "$script" --uninstall)
echo "$out"
ok "program removed" not test -e /usr/local/lib/web-stream
ok "unit removed" not test -e /etc/systemd/system/web-stream.service
ok "udev rule removed" not test -e /etc/udev/rules.d/70-web-stream-uinput.rules
ok "user removed" not id web-stream
ok "service stopped" not active
ok "nothing listens" test "$(session)" = 000
ok "data kept" as_root test -d /var/lib/web-stream/network

echo "== reinstall over kept data, then purge"
out=$(run_installer sh "$script")
ok "reinstalled" wait_host
ok "kept data handed to the new user" test "$(as_root stat -c %U /var/lib/web-stream/network)" = web-stream
out=$(run_installer sh "$script" --uninstall --purge)
echo "$out"
ok "data removed" not as_root test -e /var/lib/web-stream

if [ -n "$fake" ] && [ "$failures" -gt 0 ]; then
    as_root tail -n 40 /var/log/web-stream-fake.log
elif [ "$failures" -gt 0 ]; then
    as_root journalctl -u web-stream --no-pager -n 60
fi
echo "$failures failure(s)"
[ "$failures" = 0 ]
