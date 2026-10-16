#!/bin/sh
set -eu

if [ "$(uname -s)" != "Darwin" ]; then
    echo "This script creates a macOS app bundle. Use cargo build --release on other platforms." >&2
    exit 1
fi

cd "$(dirname "$0")/.."
profile="${1:-release}"
case "$profile" in
    release) cargo build --release --locked ;;
    debug) cargo build --locked ;;
    *) echo "Usage: sh scripts/bundle-macos.sh [release|debug]" >&2; exit 1 ;;
esac

bundle="target/$profile/LoudTrace.app"
mkdir -p "$bundle/Contents/MacOS"
cp "target/$profile/loudtrace" "$bundle/Contents/MacOS/loudtrace"
cat > "$bundle/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>CFBundleExecutable</key><string>loudtrace</string>
    <key>CFBundleIdentifier</key><string>org.loudtrace.desktop</string>
    <key>CFBundleName</key><string>LoudTrace</string>
    <key>CFBundleDisplayName</key><string>LoudTrace</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
codesign --force --sign - "$bundle"
echo "$bundle"
