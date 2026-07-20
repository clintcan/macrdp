#!/bin/bash
# Build the macrdp black shield-window helper (`macrdpshield`): swift build the SwiftPM
# executable and code-sign it. Unlike the controller, this is NOT a .app bundle —
# it's a plain executable that ships embedded in macrdp.app/Contents/Resources/
# (packaging/make-app.sh does the embed) and that macrdp spawns to cover each physical
# panel with an opaque black window (--shield-primary).
#
# Most builds don't need to run this directly — make-app.sh builds + embeds the
# helper itself. Use it for a standalone signed copy, or to drop one into an
# already-installed app:
#   gui/make-shield-helper.sh                                  # build + sign -> gui/.build/release/macrdpshield
#   INSTALL_INTO=/Applications/macrdp.app gui/make-shield-helper.sh   # also copy into that app
#
# Env overrides:
#   CODESIGN_IDENTITY="-"     # "-" = ad-hoc; or a Developer ID name
#   INSTALL_INTO=<app>        # copy the signed helper into <app>/Contents/Resources/
set -euo pipefail

GUI_DIR="$(cd "$(dirname "$0")" && pwd)"
IDENTITY="${CODESIGN_IDENTITY:--}"
# Ad-hoc ("-") can't use a secure timestamp; a real Developer ID must (notarization).
if [ "$IDENTITY" = "-" ]; then TS="--timestamp=none"; else TS="--timestamp"; fi

echo "==> swift build -c release --product macrdpshield (identity: $IDENTITY)"
( cd "$GUI_DIR" && swift build -c release --product macrdpshield )
BIN="$GUI_DIR/.build/release/macrdpshield"
[ -x "$BIN" ] || { echo "build produced no binary at $BIN" >&2; exit 1; }

echo "==> codesign (hardened runtime, ts: $TS)"
codesign --force --options runtime $TS -s "$IDENTITY" "$BIN"
codesign --verify --strict "$BIN"
echo "Built + signed: $BIN"

if [ -n "${INSTALL_INTO:-}" ]; then
    DEST="$INSTALL_INTO/Contents/Resources/macrdpshield"
    cp "$BIN" "$DEST"
    chmod +x "$DEST"
    codesign --force --options runtime $TS -s "$IDENTITY" "$DEST"
    echo "Installed into: $DEST  (re-sign the app bundle if its seal now mismatches)"
fi
