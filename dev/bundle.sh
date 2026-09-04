#!/usr/bin/env bash
#
# Builds Slate as a macOS .app and installs it to /Applications.
#
# Usage: dev/bundle.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
APP="target/Slate.app"
INSTALLED="/Applications/Slate.app"

cargo build --release

rm -rf "$APP" target/Slate.iconset
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

swift dev/icon.swift target/Slate.iconset
iconutil -c icns target/Slate.iconset -o "$APP/Contents/Resources/Slate.icns"
rm -rf target/Slate.iconset
# Capitalised: the menu bar, Force Quit and Activity Monitor all name the app
# after its executable, not after CFBundleName.
cp target/release/slate "$APP/Contents/MacOS/Slate"

# CFBundleIdentifier is what the Keychain scopes saved profile passwords to.
# Changing it orphans every password already stored.
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key><string>Slate</string>
	<key>CFBundleDisplayName</key><string>Slate</string>
	<key>CFBundleIdentifier</key><string>com.shayanabbas.slate</string>
	<key>CFBundleExecutable</key><string>Slate</string>
	<key>CFBundleIconFile</key><string>Slate</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>${VERSION}</string>
	<key>CFBundleVersion</key><string>${VERSION}</string>
	<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
	<key>LSMinimumSystemVersion</key><string>12.0</string>
	<key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
	<key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
plutil -lint -s "$APP/Contents/Info.plist"

# An arm64 binary will not launch without a signature, and copying it into the
# bundle invalidates the one rustc left behind. SLATE_SIGN_ID takes a real
# identity when there is one to distribute under, and dev/identity.sh's
# self-signed one is what keeps the Keychain from re-asking after every
# rebuild. Ad-hoc is the fallback, and it runs -- it just prompts.
DEV_IDENTITY="Slate Dev Signing"
if [[ -z "${SLATE_SIGN_ID:-}" ]] &&
  security find-certificate -c "$DEV_IDENTITY" >/dev/null 2>&1; then
  SLATE_SIGN_ID="$DEV_IDENTITY"
fi
codesign --force --sign "${SLATE_SIGN_ID:--}" "$APP"
codesign --verify --strict "$APP"

rm -rf "$INSTALLED"
ditto "$APP" "$INSTALLED"

# Finder and the Dock both cache icons per bundle path, so a swapped icon does
# not show up until the mtime moves and the Dock restarts.
touch "$INSTALLED"
killall Dock 2>/dev/null || true

echo "installed $INSTALLED (v$VERSION)"
