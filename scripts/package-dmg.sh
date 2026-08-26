#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

APP="mini-mdr"
BUNDLE_DIR="target/release/bundle/macOS/${APP}.app"
DMG_NAME="${APP}-macos-amd64.dmg"

rm -rf "$BUNDLE_DIR"
mkdir -p "${BUNDLE_DIR}/Contents/MacOS"
mkdir -p "${BUNDLE_DIR}/Contents/Resources"

cp target/release/${APP} "${BUNDLE_DIR}/Contents/MacOS/${APP}"
cp resources/icon.png "${BUNDLE_DIR}/Contents/Resources/icon.png"

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cat > "${BUNDLE_DIR}/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>${APP}</string>
    <key>CFBundleIdentifier</key><string>com.mini-mdr.app</string>
    <key>CFBundleName</key><string>${APP}</string>
    <key>CFBundleVersion</key><string>${VERSION}</string>
    <key>CFBundleShortVersionString</key><string>${VERSION}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleIconFile</key><string>icon</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSUIElement</key><true/>
</dict>
</plist>
EOF

rm -f "$DMG_NAME"
hdiutil create -volname "${APP}" -srcfolder "$BUNDLE_DIR" -ov -format UDZO "$DMG_NAME"
echo "dmg: ${DMG_NAME}"
