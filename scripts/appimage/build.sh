#!/usr/bin/env bash
# Builds dist/Gitust-<version>-x86_64.AppImage from target/release/gitust.
#
# Run it where the binary was built: in CI that's an AlmaLinux 8 container
# (glibc 2.28), so the AppImage runs on any distribution from 2018 on.
#
# winit loads the window system libraries with dlopen, so they are invisible
# to ldd and to ldd-based tools like linuxdeploy: an AppImage built that way
# aborts on systems without, say, libxkbcommon-x11. This script bundles each
# library in assets/linux/appimage-libs.txt plus everything it needs except
# glibc into usr/lib/fallback, and src/linux.rs loads a bundled copy only
# when the system has none, so the host's own libraries always win.
#
# Usage: scripts/appimage/build.sh VERSION [OUT_DIR]
# Needs: readelf (binutils), patchelf, curl, ldconfig, and the libraries
# themselves (see the container setup in .github/workflows/linux.yml).
set -euo pipefail

version=${1:?usage: build.sh VERSION [OUT_DIR]}
out=${2:-dist}
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"

# Pinned tools, checked by hash: appimagetool and the static type2 runtime
# (no libfuse2 needed on the user's system).
APPIMAGETOOL_URL=https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage
APPIMAGETOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
RUNTIME_URL=https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64
RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d

# glibc itself and libgcc_s are always the host's (AppImage excludelist).
HOST_ONLY='^(ld-linux.*|libc|libm|libmvec|libdl|libpthread|librt|libresolv|libutil|libanl|libgcc_s)\.so'

needed() { readelf -dW "$1" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'; }
glibc_max() { objdump -T "$@" 2>/dev/null | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' | sort -uV | tail -n 1; }

bin=target/release/gitust
[ -x "$bin" ] || { echo "error: $bin is missing; build it first" >&2; exit 1; }

echo "== The binary links only the C library; everything else is loaded with dlopen"
needed "$bin" | sed 's/^/  /'
extra=$(needed "$bin" | grep -vE "$HOST_ONLY" || true)
if [ -n "$extra" ]; then
  echo "error: gitust links libraries the AppImage doesn't handle: $extra" >&2
  echo "       (load them with dlopen, or add them to the bundle)" >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
app=$work/AppDir
fallback=$app/usr/lib/fallback
mkdir -p "$app/usr/bin" "$fallback" "$app/usr/share/applications" \
  "$app/usr/share/icons/hicolor/512x512/apps" "$app/usr/share/metainfo" "$out"

install -m 755 "$bin" "$app/usr/bin/gitust"
install -m 644 assets/linux/gitust.desktop "$app/usr/share/applications/gitust.desktop"
install -m 644 assets/gitust-512.png "$app/usr/share/icons/hicolor/512x512/apps/gitust.png"
date=$(git log -1 --format=%cs 2>/dev/null || date -u +%F)
sed -e "s/@VERSION@/$version/" -e "s/@DATE@/$date/" assets/linux/io.github.Layfate.Gitust.appdata.xml \
  > "$app/usr/share/metainfo/io.github.Layfate.Gitust.appdata.xml"
cp assets/linux/gitust.desktop "$app/gitust.desktop"
cp assets/gitust-512.png "$app/gitust.png"
ln -s gitust.png "$app/.DirIcon"
# libxkbcommon needs its data folder to exist, though X11 and Wayland send the
# keymap themselves; src/linux.rs points it here on systems without one.
mkdir -p "$app/usr/share/X11/xkb"
echo "Empty on purpose: see use_xkb_stand_in in Gitust's src/linux.rs." > "$app/usr/share/X11/xkb/README"
# Slint's own fallback font (OFL), for systems with no fonts at all (src/linux.rs).
slint_common=$(dirname "$(cargo metadata --format-version 1 --locked \
  | jq -r '.packages[] | select(.name == "i-slint-common") | .manifest_path')")
mkdir -p "$app/usr/share/fonts/gitust"
cp "$slint_common/sharedfontique/Inter-VariableFont.ttf" "$slint_common/sharedfontique/Inter-VariableFont.ttf.license" \
  "$app/usr/share/fonts/gitust/"
# The binary is the entry point, so /proc/self/exe is usr/bin/gitust and
# src/linux.rs finds usr/lib/fallback next to it. No LD_LIBRARY_PATH: it
# would leak into the git processes Gitust starts.
ln -s usr/bin/gitust "$app/AppRun"

echo "== Bundling the dlopen'd window system libraries and their dependencies"
resolve() { ldconfig -p | awk -v n="$1" '$1 == n && /x86-64/ { print $NF; exit }'; }
mapfile -t queue < <(grep -vE '^\s*(#|$)' assets/linux/appimage-libs.txt | awk '{ print $2 }')
while [ ${#queue[@]} -gt 0 ]; do
  so=${queue[0]}
  queue=("${queue[@]:1}")
  [[ $so =~ $HOST_ONLY ]] && continue
  [ -e "$fallback/$so" ] && continue
  path=$(resolve "$so")
  [ -n "$path" ] || { echo "error: $so isn't installed on the build system" >&2; exit 1; }
  cp -L "$path" "$fallback/$so"
  chmod 644 "$fallback/$so"
  mapfile -t -O "${#queue[@]}" queue < <(needed "$fallback/$so")
done
# Each bundled library finds its bundled dependencies next to itself; ones the
# host already loaded (by soname) are shared instead.
for f in "$fallback"/*; do patchelf --set-rpath '$ORIGIN' "$f"; done
for f in "$fallback"/*; do printf '  %-28s %6s KB  needs %s\n' "$(basename "$f")" $(($(stat -c %s "$f") / 1024)) "$(needed "$f" | tr '\n' ' ')"; done

echo "== Newest glibc symbol any file needs: $(glibc_max "$bin" "$fallback"/*)"

echo "== Packing"
curl -fsSL --retry 3 -o "$work/appimagetool" "$APPIMAGETOOL_URL"
curl -fsSL --retry 3 -o "$work/runtime" "$RUNTIME_URL"
echo "$APPIMAGETOOL_SHA256  $work/appimagetool" | sha256sum -c -
echo "$RUNTIME_SHA256  $work/runtime" | sha256sum -c -
chmod +x "$work/appimagetool"
target="$out/Gitust-$version-x86_64.AppImage"
# The containers have no FUSE, so appimagetool unpacks itself to run.
ARCH=x86_64 VERSION=$version APPIMAGE_EXTRACT_AND_RUN=1 \
  "$work/appimagetool" --no-appstream --runtime-file "$work/runtime" "$app" "$target"
ls -la "$target"
