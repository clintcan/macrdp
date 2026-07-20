#!/bin/bash
# Build macrdp.app — a stably-signed bundle with the binary as a co-signed
# helper at a fixed path, so the Screen Recording / Accessibility TCC grants
# survive rebuilds. Designed for personal use, but the bundle layout is also
# the foundation a future menu-bar GUI controller would spawn.
#
# Usage:
#   packaging/make-app.sh                 # build + bundle + ad-hoc sign + install
#
# Env overrides:
#   APP_DIR=/Applications                 # where to install (default /Applications)
#   CODESIGN_IDENTITY="-"                 # "-" = ad-hoc; or a Developer ID name
#   SKIP_BUILD=1                          # reuse an existing release binary
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG_DIR="$REPO_ROOT/packaging"
APP_DIR="${APP_DIR:-/Applications}"
# Signing identity. Ad-hoc ("-") keys TCC to the binary's cdhash, so the
# Screen Recording / Accessibility grants die on EVERY rebuild — the "grants
# survive rebuilds" promise needs a stable certificate identity. When no
# CODESIGN_IDENTITY is given, prefer a local self-signed code-signing cert
# named "macrdp-dev" if one exists (create once via Keychain Access →
# Certificate Assistant, or openssl + `security import`), falling back to
# ad-hoc only when there's nothing better.
if [ -n "${CODESIGN_IDENTITY:-}" ]; then
    IDENTITY="$CODESIGN_IDENTITY"
elif security find-identity -v -p codesigning 2>/dev/null | grep -q '"macrdp-dev"'; then
    IDENTITY="macrdp-dev"
else
    IDENTITY="-"
fi
# Bundle-ID prefix (reverse-DNS of the publishing entity). MUST match what
# install-launchagent.sh and gui/make-tray-app.sh use, or the controller will
# target the wrong LaunchAgent label.
BUNDLE_PREFIX="${BUNDLE_PREFIX:-com.clintcan}"
BUNDLE_ID="$BUNDLE_PREFIX.macrdp"

VERSION="$(grep -m1 '^version' "$REPO_ROOT/Cargo.toml" | cut -d'"' -f2)"
[ -n "$VERSION" ] || { echo "could not read version from Cargo.toml" >&2; exit 1; }

echo "==> macrdp.app v$VERSION  (id: $BUNDLE_ID, identity: $IDENTITY, install: $APP_DIR)"

# 1. Build the release binary (native target).
if [ "${SKIP_BUILD:-0}" != "1" ]; then
    echo "==> cargo build --release"
    ( cd "$REPO_ROOT" && cargo build --release )
fi
BIN="$REPO_ROOT/target/release/macrdp"
[ -x "$BIN" ] || { echo "missing release binary at $BIN (unset SKIP_BUILD?)" >&2; exit 1; }

# 2. Assemble the bundle in a staging dir under target/ (already gitignored;
#    dist/ holds the tracked install scripts, not build output).
STAGE="$REPO_ROOT/target/macrdp.app"
echo "==> staging bundle at $STAGE"
rm -rf "$STAGE"
mkdir -p "$STAGE/Contents/MacOS" "$STAGE/Contents/Resources"

sed -e "s/__VERSION__/$VERSION/g" -e "s#__BUNDLE_ID__#$BUNDLE_ID#g" \
    "$PKG_DIR/Info.plist" > "$STAGE/Contents/Info.plist"
cp "$BIN" "$STAGE/Contents/MacOS/macrdp"
chmod +x "$STAGE/Contents/MacOS/macrdp"
# No wrapper script: the LaunchAgent runs this signed binary directly with
# `--config` (the binary reads config.env itself). That gives macOS Background
# Task Management a stable Developer-ID identity to approve once, instead of an
# unsigned wrapper it re-flags on every rebuild.

# 2b. App icon (optional): drop packaging/macrdp.png or packaging/icon.png
#     (square, ideally 1024×1024) to brand the bundle. Done before signing so
#     the icon + Info.plist key are sealed.
ICON_SRC=""
for c in "$PKG_DIR/macrdp.png" "$PKG_DIR/icon.png"; do [ -f "$c" ] && { ICON_SRC="$c"; break; }; done
if [ -n "$ICON_SRC" ]; then
    "$PKG_DIR/make-icns.sh" "$ICON_SRC" "$STAGE/Contents/Resources/AppIcon.icns"
    /usr/libexec/PlistBuddy -c 'Add :CFBundleIconFile string AppIcon' "$STAGE/Contents/Info.plist" \
        2>/dev/null || /usr/libexec/PlistBuddy -c 'Set :CFBundleIconFile AppIcon' "$STAGE/Contents/Info.plist"
    echo "==> app icon: $(basename "$ICON_SRC")"
fi

# Signing timestamp flag: ad-hoc ("-") can't use a secure timestamp, and the
# local self-signed "macrdp-dev" identity doesn't need one (it's a network
# round-trip per sign and notarization is off the table anyway); a real
# Developer ID must have it (notarization requires it). Shared by the IFD
# bundle + app.
if [ "$IDENTITY" = "-" ] || [ "$IDENTITY" = "macrdp-dev" ]; then TS="--timestamp=none"; else TS="--timestamp"; fi

# Optional: provisioning profile + entitlements (USB-redirection builds only).
# PROVISION_PROFILE=<path.provisionprofile> embeds the profile and signs the main
# binary + app with packaging/macrdp.entitlements (override via ENTITLEMENTS=).
# Unset (the normal build) → no profile, no entitlements, byte-identical signing.
# The entitlement set MUST be a subset of the profile's, or codesign fails.
PROVISION_PROFILE="${PROVISION_PROFILE:-}"
ENTITLEMENTS="${ENTITLEMENTS:-}"
ENT_ARG=""
if [ -n "$PROVISION_PROFILE" ]; then
    [ -f "$PROVISION_PROFILE" ] || { echo "PROVISION_PROFILE not found: $PROVISION_PROFILE" >&2; exit 1; }
    [ "$IDENTITY" != "-" ] || { echo "PROVISION_PROFILE needs a real CODESIGN_IDENTITY (not ad-hoc)" >&2; exit 1; }
    [ -n "$ENTITLEMENTS" ] || ENTITLEMENTS="$PKG_DIR/macrdp.entitlements"
fi
if [ -n "$ENTITLEMENTS" ]; then
    [ -f "$ENTITLEMENTS" ] || { echo "ENTITLEMENTS not found: $ENTITLEMENTS" >&2; exit 1; }
    ENT_ARG="--entitlements $ENTITLEMENTS"
fi

# 2c. Embed the PC/SC IFD handler bundle (smart-card redirection,
#     --enable-smartcard-redirection). It ships inside the app so it travels in
#     the DMG; packaging/install-ifd-handler.sh later copies it (privileged) to
#     /usr/local/libexec/SmartCardServices/drivers. Built from the standalone
#     ifd-handler crate (not part of the macrdp cargo package).
if [ "${SKIP_BUILD:-0}" != "1" ]; then
    echo "==> cargo build --release (ifd-handler cdylib)"
    ( cd "$REPO_ROOT" && cargo build --release --manifest-path ifd-handler/Cargo.toml )
fi
IFD_DYLIB="$REPO_ROOT/ifd-handler/target/release/libifd_macrdp.dylib"
if [ -f "$IFD_DYLIB" ]; then
    IFD_BUNDLE="$STAGE/Contents/Resources/ifd-macrdp.bundle"
    mkdir -p "$IFD_BUNDLE/Contents/MacOS"
    sed -e "s/__VERSION__/$VERSION/g" -e "s#__BUNDLE_ID__#$BUNDLE_ID.ifd#g" \
        "$PKG_DIR/ifd-Info.plist" > "$IFD_BUNDLE/Contents/Info.plist"
    cp "$IFD_DYLIB" "$IFD_BUNDLE/Contents/MacOS/libifd_macrdp.dylib"
    # Sign the loadable dylib then the nested bundle with the app's identity +
    # flags, so it passes notarization and slotd loads it (slotd loads
    # third-party IFD drivers regardless of hardened-runtime/library-validation).
    codesign --force --options runtime $TS -s "$IDENTITY" "$IFD_BUNDLE/Contents/MacOS/libifd_macrdp.dylib"
    codesign --force --options runtime $TS -s "$IDENTITY" "$IFD_BUNDLE"
    # Ship the privileged installer alongside it so DMG users can run
    #   /Applications/macrdp.app/Contents/Resources/install-ifd-handler.sh
    # plus the USB-trigger picker the installer invokes (must sit next to it).
    cp "$PKG_DIR/install-ifd-handler.sh" "$STAGE/Contents/Resources/install-ifd-handler.sh"
    cp "$PKG_DIR/select-usb-trigger.sh" "$STAGE/Contents/Resources/select-usb-trigger.sh"
    chmod +x "$STAGE/Contents/Resources/install-ifd-handler.sh" \
             "$STAGE/Contents/Resources/select-usb-trigger.sh"
    echo "==> embedded ifd-macrdp.bundle (smart-card IFD handler) + installer + USB picker"
else
    echo "==> WARNING: ifd-handler dylib not found; smart-card handler NOT embedded (unset SKIP_BUILD?)" >&2
fi

# 2d. Embed the app-switcher HUD helper (--app-switcher-hud). A small Swift
#     executable built from gui/; macrdp spawns it from Contents/Resources/macrdphud
#     (see locate_hud_helper in src/main.rs). Signed before the outer bundle is
#     sealed so the deep signature stays valid + it passes notarization.
if [ "${SKIP_BUILD:-0}" != "1" ]; then
    echo "==> swift build -c release (macrdphud)"
    ( cd "$REPO_ROOT/gui" && swift build -c release --product macrdphud )
fi
HUD_BIN="$REPO_ROOT/gui/.build/release/macrdphud"
if [ -f "$HUD_BIN" ]; then
    cp "$HUD_BIN" "$STAGE/Contents/Resources/macrdphud"
    chmod +x "$STAGE/Contents/Resources/macrdphud"
    codesign --force --options runtime $TS -s "$IDENTITY" "$STAGE/Contents/Resources/macrdphud"
    echo "==> embedded macrdphud (app-switcher HUD helper)"
else
    echo "==> WARNING: macrdphud not found; app-switcher HUD NOT embedded (unset SKIP_BUILD?)" >&2
fi

# 2e. Embed the black shield-window helper (--shield-primary). Same shape as the
#     HUD helper: a small Swift executable macrdp spawns from
#     Contents/Resources/macrdpshield (see locate_shield_helper in src/main.rs),
#     signed before the outer bundle seal.
if [ "${SKIP_BUILD:-0}" != "1" ]; then
    echo "==> swift build -c release (macrdpshield)"
    ( cd "$REPO_ROOT/gui" && swift build -c release --product macrdpshield )
fi
SHIELD_BIN="$REPO_ROOT/gui/.build/release/macrdpshield"
if [ -f "$SHIELD_BIN" ]; then
    cp "$SHIELD_BIN" "$STAGE/Contents/Resources/macrdpshield"
    chmod +x "$STAGE/Contents/Resources/macrdpshield"
    codesign --force --options runtime $TS -s "$IDENTITY" "$STAGE/Contents/Resources/macrdpshield"
    echo "==> embedded macrdpshield (shield-window helper)"
else
    echo "==> WARNING: macrdpshield not found; --shield-primary will REFUSE to start (unset SKIP_BUILD?)" >&2
fi

# 3. Sign the Mach-O executable, then the bundle (which seals Info.plist + the
#    Resources, including the app icon + the embedded IFD bundle).
# Embed the provisioning profile (if any) BEFORE the bundle sign so it's sealed in.
if [ -n "$PROVISION_PROFILE" ]; then
    cp "$PROVISION_PROFILE" "$STAGE/Contents/embedded.provisionprofile"
    echo "==> embedded provisioning profile"
fi
echo "==> codesign (hardened runtime, ts: $TS${ENT_ARG:+, entitlements})"
# Entitlements go on the main executable (which actually runs) and the bundle.
# The other signed items (IFD dylib/bundle, macrdphud, macrdpshield) deliberately
# get NO entitlements — only macrdp needs the USB host-controller capability.
codesign --force --options runtime $TS $ENT_ARG -s "$IDENTITY" "$STAGE/Contents/MacOS/macrdp"
codesign --force --options runtime $TS $ENT_ARG -s "$IDENTITY" "$STAGE"
codesign --verify --deep --strict "$STAGE"

# 3b. Optional notarization (NOTARIZE=1, real Developer ID + NOTARY_PROFILE).
#     Done on the staged app so the stapled ticket travels with the install copy.
if [ "${NOTARIZE:-0}" = "1" ]; then
    [ "$IDENTITY" != "-" ] || { echo "NOTARIZE=1 needs a real CODESIGN_IDENTITY (not ad-hoc)" >&2; exit 1; }
    "$PKG_DIR/notarize.sh" "$STAGE"
fi

# 4. Install to the stable path. cp -R preserves the signature.
echo "==> installing to $APP_DIR/macrdp.app"
if ! mkdir -p "$APP_DIR" 2>/dev/null || [ ! -w "$APP_DIR" ]; then
    echo "    $APP_DIR is not writable — re-run with sudo, or set APP_DIR=\$HOME/Applications" >&2
    exit 1
fi
rm -rf "$APP_DIR/macrdp.app"
cp -R "$STAGE" "$APP_DIR/macrdp.app"
codesign --verify --strict "$APP_DIR/macrdp.app"

echo
echo "Done. Installed: $APP_DIR/macrdp.app"
codesign -dv "$APP_DIR/macrdp.app" 2>&1 | sed 's/^/    /'
echo
echo "Next:"
echo "  1. Store the password once:"
echo "       security add-generic-password -s macrdp -a \"\$(id -un)\" -w 'YOUR_PASSWORD'"
echo "  2. Install + load the LaunchAgent:"
echo "       APP_DIR=\"$APP_DIR\" packaging/install-launchagent.sh"
echo "  3. Grant Screen Recording + Accessibility to macrdp.app when prompted"
echo "     (System Settings -> Privacy & Security)."
