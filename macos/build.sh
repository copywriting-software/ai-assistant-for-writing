#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$SCRIPT_DIR/AIWritingHelper.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
BIN_OUT="$MACOS_DIR/ai-writing-macos-helper"
BUNDLE_ID="com.umax.ai-writing-helper"

mkdir -p "$MACOS_DIR"

echo "Compiling macOS native helper -> $BIN_OUT"
swiftc -O "$SCRIPT_DIR/MacHelper.swift" \
  -framework AppKit \
  -framework ApplicationServices \
  -o "$BIN_OUT"

cat > "$CONTENTS_DIR/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>
    <key>CFBundleName</key>
    <string>AIWritingHelper</string>
    <key>CFBundleExecutable</key>
    <string>ai-writing-macos-helper</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>LSUIElement</key>
    <true/>
</dict>
</plist>
EOF

# Sign with a stable certificate from Keychain if available so TCC permissions persist across rebuilds.
SIGN_IDENTITY="${CODESIGN_IDENTITY:-}"
if [[ -z "$SIGN_IDENTITY" ]]; then
  SIGN_IDENTITY="$(security find-identity -v -p codesigning | sed -n 's/.*"\(.*\)".*/\1/p' | head -n 1 || true)"
fi

if [[ -n "$SIGN_IDENTITY" ]]; then
  echo "Signing $APP_DIR with identity: $SIGN_IDENTITY"
  codesign --force --deep --sign "$SIGN_IDENTITY" --identifier "$BUNDLE_ID" "$APP_DIR"
else
  echo "No Keychain signing identity found; ad-hoc signing $APP_DIR"
  codesign --force --deep --sign - --identifier "$BUNDLE_ID" "$APP_DIR"
fi

codesign --verify --verbose=2 "$APP_DIR"
echo "Built & signed successfully: $APP_DIR"
