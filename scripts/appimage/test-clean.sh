#!/usr/bin/env bash
# Starts the AppImage in a bare container image (nothing installed: no X
# libraries, no fonts, no fontconfig, no git, no CA certificates) with
# networking disabled, showing its window on the host's Xvfb display.
#
# It passes when Gitust reports a visible window (GITUST_SMOKE_TEST), exits
# cleanly, and its log has no panic and shows the offline update check was
# handled. A screenshot of the window is saved for a look.
#
# Usage: scripts/appimage/test-clean.sh APPIMAGE IMAGE [OUT_DIR]
#   e.g. scripts/appimage/test-clean.sh dist/Gitust-0.3.0-x86_64.AppImage debian:12
# Needs docker, Xvfb, xdotool and ImageMagick on the host.
set -euo pipefail

appimage=$(readlink -f "${1:?usage: test-clean.sh APPIMAGE IMAGE [OUT_DIR]}")
image=${2:?usage: test-clean.sh APPIMAGE IMAGE [OUT_DIR]}
out=$(mkdir -p "${3:-appimage-test}" && cd "${3:-appimage-test}" && pwd)
name=$(echo "$image" | tr -c 'A-Za-z0-9\n' '-')

export DISPLAY=:99
if ! xdpyinfo >/dev/null 2>&1; then
  Xvfb :99 -screen 0 1280x800x24 -ac >/dev/null 2>&1 &
  xvfb=$!
  trap 'kill "$xvfb" 2>/dev/null || true' EXIT
  sleep 2
fi

home=$(mktemp -d)
chmod 777 "$home"
docker pull -q "$image"
echo "== $image: $(docker run --rm "$image" sh -c '(ldd --version 2>&1 || true) | head -n 1')"
docker run --rm --network none --user "$(id -u):$(id -g)" \
  -v /tmp/.X11-unix:/tmp/.X11-unix -v "$appimage:/opt/Gitust.AppImage:ro" -v "$home:/home/test" \
  -e DISPLAY=:99 -e HOME=/home/test -e APPIMAGE_EXTRACT_AND_RUN=1 -e GITUST_SMOKE_TEST=10 \
  "$image" /opt/Gitust.AppImage >"$out/$name-stdout.txt" 2>&1 &
app=$!
sleep 7
import -window root "$out/$name.png" || true
status=0
wait "$app" || status=$?
log="$home/.local/share/gitust/gitust.log"
cat "$out/$name-stdout.txt"
cp "$log" "$out/$name-gitust.log" 2>/dev/null || true
cat "$log" 2>/dev/null || true

[ "$status" -eq 0 ] || { echo "error: Gitust exited with status $status" >&2; exit 1; }
grep -q 'window visible = true' "$out/$name-stdout.txt" || { echo "error: no visible window" >&2; exit 1; }
if grep -q '\[PANIC\]' "$log"; then echo "error: Gitust panicked" >&2; exit 1; fi
grep -q 'update check skipped' "$log" || { echo "error: no sign the offline update check was handled" >&2; exit 1; }
echo "PASS: $image"
