#!/usr/bin/env bash
# Checks the Linux packages in a dist folder: packaging/check-linux.sh <dist dir>
#
# Needs dpkg-deb and rpm. Prints "ok", or a message on stderr and exits 1.
set -euo pipefail

dist=${1:?usage: packaging/check-linux.sh <dist dir>}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
expected="$here/linux/dev.nikethai.hyades.desktop"
# Every package installs its desktop entry under this name, and the
# AppStream metainfo's launchable names it.
entry=hyades.desktop
metainfo=usr/share/metainfo/dev.nikethai.hyades.metainfo.xml

fail() {
  echo "check-linux: $*" >&2
  exit 1
}

[ -d "$dist" ] || fail "$dist is not a folder"
dist=$(cd "$dist" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
shopt -s nullglob

# The value of key $2 in desktop file $1 ("" if absent).
desktop_value() {
  sed -n "s/^$2=//p" "$1" | head -n 1
}

# MimeType of desktop file $1 as sorted lines.
mime_types() {
  desktop_value "$1" MimeType | tr ';' '\n' | sed '/^$/d' | sort
}

# Desktop file $1 (described as $2) launches Hyades for magnet links and
# .torrent files, like packaging/linux/dev.nikethai.hyades.desktop.
check_desktop() {
  local file=$1 what=$2
  [ -f "$file" ] || fail "$what: no usr/share/applications/$entry"
  local name exec mime
  name=$(desktop_value "$file" Name)
  exec=$(desktop_value "$file" Exec)
  mime=$(mime_types "$file")
  [ "$name" = "Hyades" ] || fail "$what: Name is '$name', not Hyades"
  grep -Eqx 'hyades %[uU]' <<<"$exec" ||
    fail "$what: Exec is '$exec', not 'hyades %u' or 'hyades %U'"
  for type in x-scheme-handler/magnet application/x-bittorrent; do
    grep -qx "$type" <<<"$mime" || fail "$what: MimeType lacks $type"
  done
  [ "$mime" = "$(mime_types "$expected")" ] ||
    fail "$what: MimeType differs from packaging/linux/dev.nikethai.hyades.desktop"
}

# The metainfo under unpacked package root $1 (described as $2) names the
# package's desktop entry as its launchable.
check_metainfo() {
  local root=$1 what=$2
  [ -f "$root/$metainfo" ] || fail "$what: no $metainfo"
  local launchable
  launchable=$(sed -n 's|.*<launchable type="desktop-id">\(.*\)</launchable>.*|\1|p' "$root/$metainfo")
  [ "$launchable" = "$entry" ] ||
    fail "$what: metainfo launchable is '$launchable', not $entry"
  [ -f "$root/usr/share/applications/$launchable" ] ||
    fail "$what: metainfo launchable $launchable is not in the package"
}

check_desktop "$expected" "packaging/linux/dev.nikethai.hyades.desktop"

# deb
debs=("$dist"/*.deb)
[ ${#debs[@]} -gt 0 ] || fail "no .deb found in $dist"
for deb in "${debs[@]}"; do
  name=$(basename "$deb")
  contents=$(dpkg-deb -c "$deb") || fail "$name: dpkg-deb -c failed"
  # Paths with or without a leading "./", depending on the tool.
  list=$(awk '{ print $NF }' <<<"$contents" | sed 's|^\./||')
  grep -qx 'usr/bin/hyades' <<<"$list" || fail "$name: no usr/bin/hyades"
  grep -qx "usr/share/applications/$entry" <<<"$list" ||
    fail "$name: no usr/share/applications/$entry"
  grep -Eqx 'usr/share/icons/hicolor/256x256/apps/[^/]+\.png' <<<"$list" ||
    fail "$name: no 256x256 icon"
  rm -rf "$work/deb"
  dpkg-deb -x "$deb" "$work/deb" || fail "$name: dpkg-deb -x failed"
  check_desktop "$work/deb/usr/share/applications/$entry" "$name"
  check_metainfo "$work/deb" "$name"
done

# rpm
command -v rpm >/dev/null || fail "rpm is not installed (apt-get install rpm)"
rpms=("$dist"/*.rpm)
[ ${#rpms[@]} -gt 0 ] || fail "no .rpm found in $dist"
for pkg in "${rpms[@]}"; do
  name=$(basename "$pkg")
  list=$(rpm -qlp "$pkg" 2>/dev/null) || fail "$name: rpm -qlp failed"
  grep -qx '/usr/bin/hyades' <<<"$list" || fail "$name: no /usr/bin/hyades"
  grep -qx "/usr/share/applications/$entry" <<<"$list" ||
    fail "$name: no /usr/share/applications/$entry"
done

# AppImage (--appimage-extract needs no FUSE)
appimages=("$dist"/*.AppImage)
[ ${#appimages[@]} -gt 0 ] || fail "no .AppImage found in $dist"
for app in "${appimages[@]}"; do
  name=$(basename "$app")
  rm -rf "$work/appimage"
  mkdir "$work/appimage"
  chmod u+x "$app"
  (cd "$work/appimage" && "$app" --appimage-extract >/dev/null) ||
    fail "$name: --appimage-extract failed"
  root="$work/appimage/squashfs-root"
  [ -f "$root/usr/bin/hyades" ] || fail "$name: no usr/bin/hyades"
  check_desktop "$root/usr/share/applications/$entry" "$name"
  check_metainfo "$root" "$name"
done

echo ok
