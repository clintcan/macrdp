#!/bin/bash
# Build a signed (+ optionally notarized) distribution DMG containing the given
# .app bundle(s) plus an /Applications drop-link for drag-to-install.
#
# The apps SHOULD already be signed + notarized + stapled (run make-app.sh /
# make-tray-app.sh with NOTARIZE=1 first). This wraps them, signs the DMG, and
# notarizes + staples the DMG itself so the download passes Gatekeeper offline.
#
# Usage:
#   packaging/make-dmg.sh [App.app ...]
#     With no args, includes target/macrdp.app and target/macrdpController.app
#     if present.
#
# Env:
#   CODESIGN_IDENTITY="-"            "-" = ad-hoc (won't pass Gatekeeper); or a
#                                    "Developer ID Application: …" identity.
#   NOTARIZE=1 + NOTARY_PROFILE=<p>  notarize + staple the DMG.
#   OUT_DIR=<dir>                    output dir (default: target/, gitignored).
#   DMG_NAME=<name.dmg>              output filename (default: macrdp-<version>.dmg).
#   VOL_NAME=<name>                  mounted volume name (default: "macrdp <version>").
#   ALLOW_NO_CAMERA=1                accept a controller without the camera system
#                                    extension (see the check below).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG_DIR="$REPO_ROOT/packaging"
IDENTITY="${CODESIGN_IDENTITY:--}"
OUT_DIR="${OUT_DIR:-$REPO_ROOT/target}"
VERSION="$(grep -m1 '^version' "$REPO_ROOT/Cargo.toml" | cut -d'"' -f2)"
[ -n "$VERSION" ] || { echo "could not read version from Cargo.toml" >&2; exit 1; }

# Collect app bundles: explicit args, else the staged builds under target/.
apps=("$@")
if [ ${#apps[@]} -eq 0 ]; then
    for a in "$REPO_ROOT/target/macrdp.app" "$REPO_ROOT/target/macrdpController.app"; do
        [ -d "$a" ] && apps+=("$a")
    done
fi
[ ${#apps[@]} -gt 0 ] || { echo "no .app bundles given or found in target/ — build them first" >&2; exit 1; }
for a in "${apps[@]}"; do [ -d "$a" ] || { echo "not a bundle: $a" >&2; exit 1; }; done

# The controller must carry the camera system extension, embedded and allowed
# by its entitlement and profile, or "Enable macrdp Camera" fails with
# "Missing entitlement com.apple.developer.system-extension.install". The
# v0.11.2 DMG shipped without it (#319): its controller was built without
# CAMERA_EXTENSION=1. Build it as docs/camera-extension-setup.md says.
if [ "${ALLOW_NO_CAMERA:-0}" != "1" ]; then
    for a in "${apps[@]}"; do
        case "$(basename "$a")" in *Controller*.app) ;; *) continue ;; esac
        problems=()
        ls "$a"/Contents/Library/SystemExtensions/*camera*.systemextension >/dev/null 2>&1 \
            || problems+=("no camera system extension in Contents/Library/SystemExtensions")
        [ -f "$a/Contents/embedded.provisionprofile" ] \
            || problems+=("no Contents/embedded.provisionprofile")
        codesign -d --entitlements - "$a" 2>/dev/null | grep -q "system-extension.install" \
            || problems+=("not signed with com.apple.developer.system-extension.install")
        if [ ${#problems[@]} -gt 0 ]; then
            echo "$a can't enable macrdp Camera:" >&2
            printf '  - %s\n' "${problems[@]}" >&2
            echo "Build it with CAMERA_EXTENSION=1 and both profiles (docs/camera-extension-setup.md), or set ALLOW_NO_CAMERA=1." >&2
            exit 1
        fi
    done
fi

DMG_NAME="${DMG_NAME:-macrdp-$VERSION.dmg}"
VOL_NAME="${VOL_NAME:-macrdp $VERSION}"
DMG="$OUT_DIR/$DMG_NAME"
mkdir -p "$OUT_DIR"

echo "==> DMG v$VERSION (identity: $IDENTITY) including:"
for a in "${apps[@]}"; do echo "      - $(basename "$a")"; done

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
STAGE="$WORK/dmg"
mkdir -p "$STAGE"
for a in "${apps[@]}"; do cp -R "$a" "$STAGE/"; done
ln -s /Applications "$STAGE/Applications"   # drag-to-install target

rm -f "$DMG"
# Build a read-write DMG, lay out the icons in Finder (best-effort — needs the
# build process to have Finder automation permission; falls back to an unstyled
# but functional layout if that's denied), then convert to compressed read-only.
# Optional background: packaging/dmg-background.png.
RW="$WORK/rw.dmg"
echo "==> hdiutil create (rw)"
hdiutil create -volname "$VOL_NAME" -srcfolder "$STAGE" -ov -format UDRW "$RW" >/dev/null
BG="$REPO_ROOT/packaging/dmg-background.png"
MOUNT="$(hdiutil attach "$RW" -nobrowse -noverify -noautoopen 2>/dev/null | grep -o '/Volumes/.*' | head -1 || true)"
if [ -n "$MOUNT" ]; then
    echo "==> styling DMG window (best-effort)"
    bg_clause=""
    if [ -f "$BG" ]; then
        mkdir -p "$MOUNT/.background"
        cp "$BG" "$MOUNT/.background/bg.png"
        bg_clause='set background picture of theViewOptions to file ".background:bg.png"'
    fi
    osascript >/dev/null 2>&1 <<OSA || echo "   (Finder styling skipped — automation not permitted)"
tell application "Finder"
  tell disk "$VOL_NAME"
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set the bounds of container window to {200, 120, 800, 520}
    set theViewOptions to the icon view options of container window
    set arrangement of theViewOptions to not arranged
    set icon size of theViewOptions to 96
    $bg_clause
    try
      set position of item "macrdp.app" of container window to {150, 150}
    end try
    try
      set position of item "macrdpController.app" of container window to {150, 320}
    end try
    set position of item "Applications" of container window to {450, 230}
    update without registering applications
    delay 1
    close
  end tell
end tell
OSA
    sync
    hdiutil detach "$MOUNT" >/dev/null 2>&1 || hdiutil detach "$MOUNT" -force >/dev/null 2>&1 || true
fi
echo "==> hdiutil convert (compressed)"
hdiutil convert "$RW" -format UDZO -o "$DMG" >/dev/null

if [ "$IDENTITY" != "-" ]; then
    echo "==> codesign DMG (secure timestamp)"
    codesign --force --timestamp -s "$IDENTITY" "$DMG"
    codesign --verify --strict "$DMG"
fi

if [ "${NOTARIZE:-0}" = "1" ]; then
    [ "$IDENTITY" != "-" ] || { echo "NOTARIZE=1 needs a real CODESIGN_IDENTITY (not ad-hoc)" >&2; exit 1; }
    "$PKG_DIR/notarize.sh" "$DMG"
fi

echo
echo "DMG ready: $DMG"
ls -lh "$DMG" | awk '{print "    size: "$5}'
