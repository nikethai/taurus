#!/usr/bin/env bash
# Checks the Linux packages in a dist folder: packaging/check-linux.sh <dist dir>
#
# Needs dpkg-deb and rpm. Prints "ok", or a message on stderr and exits 1.
set -euo pipefail

dist=${1:?usage: packaging/check-linux.sh <dist dir>}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
expected="$here/linux/dev.nikethai.hyades.desktop"

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
  [ "$(desktop_value "$file" Name)" = "Hyades" ] || fail "$what: Name is not Hyades"
  desktop_value "$file" Exec | grep -Eqx 'hyades %[uU]' ||
    fail "$what: Exec is '$(desktop_value "$file" Exec)', not 'hyades %u' or 'hyades %U'"
  local mime
  mime=$(mime_types "$file")
  for type in x-scheme-handler/magnet application/x-bittorrent; do
    grep -qx "$type" <<<"$mime" || fail "$what: MimeType lacks $type"
  done
  [ "$mime" = "$(mime_types "$expected")" ] ||
    fail "$what: MimeType differs from packaging/linux/dev.nikethai.hyades.desktop"
}

# The single file in $1 (described as $2).
first_desktop() {
  local files=("$1"/*.desktop)
  [ ${#files[@]} -eq 1 ] || fail "$2: expected one .desktop file, found ${#files[@]}"
  echo "${files[0]}"
}

check_desktop "$expected" "packaging/linux/dev.nikethai.hyades.desktop"

# deb
debs=("$dist"/*.deb)
[ ${#debs[@]} -gt 0 ] || fail "no .deb found in $dist"
for deb in "${debs[@]}"; do
  name=$(basename "$deb")
  # Paths with or without a leading "./", depending on the tool.
  list=$(dpkg-deb -c "$deb" | awk '{ print $NF }' | sed 's|^\./||')
  grep -qx 'usr/bin/hyades' <<<"$list" || fail "$name: no usr/bin/hyades"
  grep -Eqx 'usr/share/applications/[^/]+\.desktop' <<<"$list" ||
    fail "$name: no usr/share/applications/*.desktop"
  grep -Eqx 'usr/share/icons/hicolor/256x256/apps/[^/]+\.png' <<<"$list" ||
    fail "$name: no 256x256 icon"
  rm -rf "$work/deb"
  dpkg-deb -x "$deb" "$work/deb"
  check_desktop "$(first_desktop "$work/deb/usr/share/applications" "$name")" "$name"
done

# rpm
command -v rpm >/dev/null || fail "rpm is not installed (apt-get install rpm)"
rpms=("$dist"/*.rpm)
[ ${#rpms[@]} -gt 0 ] || fail "no .rpm found in $dist"
for pkg in "${rpms[@]}"; do
  name=$(basename "$pkg")
  list=$(rpm -qlp "$pkg" 2>/dev/null)
  grep -qx '/usr/bin/hyades' <<<"$list" || fail "$name: no /usr/bin/hyades"
  grep -Eqx '/usr/share/applications/[^/]+\.desktop' <<<"$list" ||
    fail "$name: no /usr/share/applications/*.desktop"
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
  check_desktop "$(first_desktop "$root/usr/share/applications" "$name")" "$name"
done

echo ok
