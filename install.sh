#!/bin/sh
# Standalone install. NIRUX owns the normal suite installation.
set -eu
PREFIX=${NIRUTHEMESELECTOR_PREFIX:-${NIRUX_PREFIX:-$HOME/.local}}
BIN=$PREFIX/bin
APPS=$PREFIX/share/applications
ICONS=$PREFIX/share/icons/hicolor/256x256/apps
here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
install -d "$BIN" "$APPS" "$ICONS"
install -m 755 "$here/target/release/niruthemeselector" "$BIN/niruthemeselector"
sed "s|^Exec=niruthemeselector$|Exec=$BIN/niruthemeselector|" "$here/niruthemeselector.desktop" > "$APPS/niruthemeselector.desktop"
install -m 644 "$here/assets/niruthemeselector.png" "$ICONS/niruthemeselector.png"
printf 'installed %s\n' "$BIN/niruthemeselector"
