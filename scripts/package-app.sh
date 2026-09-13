#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
cargo build --locked
bundle=target/package/cibergit.app
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp target/aarch64-apple-darwin/debug/cibergit "$bundle/Contents/MacOS/cibergit"
cp LICENSE THIRD_PARTY_NOTICES.md "$bundle/Contents/Resources/"
cat > "$bundle/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>cibergit</string>
<key>CFBundleIdentifier</key><string>dev.cibergit.cibergit</string>
<key>CFBundleName</key><string>cibergit</string>
<key>CFBundleDisplayName</key><string>cibergit</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>15.0</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST
plutil -lint "$bundle/Contents/Info.plist"
printf 'Unsigned development app: %s/%s\n' "$PWD" "$bundle"
