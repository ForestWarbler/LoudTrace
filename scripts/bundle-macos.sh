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
    universal)
        # One binary for Apple silicon and Intel Macs.
        cargo build --release --locked --target aarch64-apple-darwin
        cargo build --release --locked --target x86_64-apple-darwin
        mkdir -p target/universal
        lipo -create -output target/universal/loudtrace \
            target/aarch64-apple-darwin/release/loudtrace \
            target/x86_64-apple-darwin/release/loudtrace
        ;;
    *) echo "Usage: sh scripts/bundle-macos.sh [release|debug|universal]" >&2; exit 1 ;;
esac

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)"
bundle="target/$profile/LoudTrace.app"
rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "target/$profile/loudtrace" "$bundle/Contents/MacOS/loudtrace"
cp assets/icons/AppIcon.icns "$bundle/Contents/Resources/AppIcon.icns"
cat > "$bundle/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>CFBundleExecutable</key><string>loudtrace</string>
    <key>CFBundleIdentifier</key><string>org.loudtrace.desktop</string>
    <key>CFBundleName</key><string>LoudTrace</string>
    <key>CFBundleDisplayName</key><string>LoudTrace</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
codesign --force --sign - "$bundle"
echo "$bundle"
