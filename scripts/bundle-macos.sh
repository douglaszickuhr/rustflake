#!/bin/sh
# Packages target/release/rustflake as rustflake.app inside dist/rustflake-<version>-macos-arm64.dmg.
# Signs with $MACOS_SIGN_IDENTITY ("Developer ID Application: …", hardened runtime) when set, ad-hoc otherwise.
# Run `cargo build --release -p rb-app` first.
set -eu
version=$(cargo pkgid -p rb-app | sed 's/.*[#@]//')
rm -rf dist
staging=dist/dmg
app=$staging/rustflake.app
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/rustflake "$app/Contents/MacOS/"

iconset=dist/AppIcon.iconset
mkdir "$iconset"
for s in 16 32 128 256 512; do
  sips -z $s $s crates/rb-app/assets/app-icon.png --out "$iconset/icon_${s}x${s}.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"

# Same identifier as scripts/dev-run.sh and the Keychain service (dev.rustflake).
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>rustflake</string>
  <key>CFBundleIdentifier</key><string>dev.rustflake</string>
  <key>CFBundleName</key><string>rustflake</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

if [ -n "${MACOS_SIGN_IDENTITY:-}" ]; then
  codesign --force --options runtime --timestamp --sign "$MACOS_SIGN_IDENTITY" "$app"
else
  codesign --force --sign - "$app"
fi

ln -s /Applications "$staging/Applications"
dmg=dist/rustflake-$version-macos-arm64.dmg
hdiutil create -volname rustflake -srcfolder "$staging" -format UDZO -ov "$dmg" >/dev/null
if [ -n "${MACOS_SIGN_IDENTITY:-}" ]; then
  codesign --sign "$MACOS_SIGN_IDENTITY" --timestamp "$dmg"
fi
echo "$dmg"
