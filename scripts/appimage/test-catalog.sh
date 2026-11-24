#!/usr/bin/env bash
# Tests an AppImage the way the AppImage catalog (appimage.github.io) does:
# its AppDir lint, C library check and screenshot check (fetched at a pinned
# commit), and a launch in firejail with no network on an Xvfb display with
# icewm, without arguments, waiting for a window like the catalog's worker.sh.
#
# Stricter than the catalog in three ways: libxkbcommon-x11 is removed from
# the system first (the library missing AppImages most often crash on), the
# app must still be running after the screenshot, and its log must show that
# the failed update check was handled and nothing panicked.
#
# Usage: scripts/appimage/test-catalog.sh Gitust-x.y.z-x86_64.AppImage [OUT_DIR]
# For an Ubuntu 22.04 GitHub runner (the catalog's own test system); uses sudo.
set -euo pipefail

appimage=$(readlink -f "${1:?usage: test-catalog.sh APPIMAGE [OUT_DIR]}")
out=$(mkdir -p "${2:-appimage-test}" && cd "${2:-appimage-test}" && pwd)
CATALOG=https://raw.githubusercontent.com/AppImage/appimage.github.io/71f4e55315c9a0fe423c8657dd0cb4f963cadf4a/code
EXCLUDELIST=https://raw.githubusercontent.com/AppImage/AppImages/19e30b276ffedf4d3b4b56bc6320f463625a74f8/excludelist

echo "== Test system"
sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
  xvfb icewm xdotool x11-utils imagemagick tesseract-ocr tesseract-ocr-eng firejail \
  desktop-file-utils libfile-mimeinfo-perl appstream file binutils >/dev/null
sudo DEBIAN_FRONTEND=noninteractive apt-get purge -y -qq libxkbcommon-x11-0 || true
if ldconfig -p | grep -q libxkbcommon-x11.so.0; then echo "error: libxkbcommon-x11 is still installed" >&2; exit 1; fi
echo "libxkbcommon-x11: not installed (the AppImage has to bring it)"

tools=$(mktemp -d)
for f in appdir-lint.sh check-libc.sh check-screenshot.sh take-screenshot.sh; do
  curl -fsSL --retry 3 -o "$tools/$f" "$CATALOG/$f"
done
curl -fsSL --retry 3 -o "$tools/excludelist" "$EXCLUDELIST"

echo "== AppDir lint (catalog's appdir-lint.sh)"
cd "$tools"
"$appimage" --appimage-extract >/dev/null
EXCLUDELIST=$tools/excludelist bash appdir-lint.sh "$tools/squashfs-root" | tee "$out/lint.txt"
appstreamcli validate --no-net --explain "$tools"/squashfs-root/usr/share/metainfo/*.xml

echo "== C library (catalog's check-libc.sh)"
bash check-libc.sh "$appimage" "$tools/squashfs-root" | tee "$out/libc.txt"
# The build container's glibc; anything newer would lock out older systems.
baseline=2.28
required=$(sed -n 's/^X-AppImage-Glibc-Required=GLIBC_//p' "$out/libc.txt")
if [ "$(printf '%s\n' "$required" "$baseline" | sort -V | tail -n 1)" != "$baseline" ]; then
  echo "error: needs glibc $required, newer than the $baseline baseline" >&2
  exit 1
fi

echo "== Launch: firejail --net=none, no arguments"
export DISPLAY=:99
Xvfb :99 -screen 0 800x600x24 >/dev/null 2>&1 &
xvfb=$!
sleep 2
mkdir -p "$HOME/.icewm"
printf 'ShowTaskBar = 0\n' > "$HOME/.icewm/preferences"
icewm >/dev/null 2>&1 &
wm=$!
data="$HOME/.local/share/gitust"
rm -rf "$data"
cleanup() { kill "${app:-}" "$wm" "$xvfb" 2>/dev/null || true; }
trap cleanup EXIT

firejail --quiet --noprofile --net=none --appimage "$appimage" >"$out/stdout.txt" 2>&1 &
app=$!
sleep 10
for _ in $(seq 30); do
  kill -0 "$app" 2>/dev/null || break
  timeout 5 xdotool search --onlyvisible --name '.' >/dev/null 2>&1 && break
  sleep 1
done
sleep 2
if ! kill -0 "$app" 2>/dev/null; then
  echo "error: Gitust exited instead of showing a window" >&2
  cat "$out/stdout.txt" "$data/gitust.log" 2>/dev/null || true
  exit 1
fi
timeout 20 xwininfo -tree -root | grep '": ("' | sed 's/^ */  /'
bash take-screenshot.sh "$out/screenshot.png"
timeout 20 import -window root "$out/screen.png"

echo "== Screenshot (catalog's check-screenshot.sh)"
bash check-screenshot.sh "$out/screenshot.png"

sleep 3
kill -0 "$app" 2>/dev/null || { echo "error: Gitust quit after showing its window" >&2; exit 1; }
kill "$app"
wait "$app" 2>/dev/null || true

echo "== Gitust's log"
cp "$data/gitust.log" "$out/gitust.log"
cat "$out/gitust.log"
if grep -q '\[PANIC\]' "$out/gitust.log"; then echo "error: Gitust panicked" >&2; exit 1; fi
grep -q 'using bundled libraries the system lacks: .*libxkbcommon-x11.so.0' "$out/gitust.log" \
  || { echo "error: the bundled libxkbcommon-x11 wasn't used" >&2; exit 1; }
grep -q 'update check skipped' "$out/gitust.log" \
  || { echo "error: no sign the offline update check was handled" >&2; exit 1; }
echo "PASS: catalog checks, window shown and kept open with no network"
