#!/bin/sh
# Builds target/release/Findest.app, so macOS treats Findest as its own app
# (its own privacy permissions, Dock icon, Launchpad, Spotlight).
#
#   scripts/bundle.sh            build the bundle
#   scripts/bundle.sh --install  also copy it to /Applications and launch it
#
# Signing: uses $FINDEST_SIGN_IDENTITY, else the first "Apple Development"
# certificate in the keychain, else an ad-hoc signature. A real certificate
# keeps the app's identity stable across rebuilds, so privacy permissions
# (Full Disk Access, cloud folders) survive. Ad-hoc signatures change on every
# build and macOS silently drops the permissions.
set -eu
cd "$(dirname "$0")/.."

cargo build --release -p findest

APP=target/release/Findest.app
VERSION=$(cargo pkgid -p findest | sed 's/.*[#@]//')
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/findest "$APP/Contents/MacOS/findest"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Findest</string>
    <key>CFBundleDisplayName</key><string>Findest</string>
    <key>CFBundleIdentifier</key><string>dev.findest.Findest</string>
    <key>CFBundleExecutable</key><string>findest</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

IDENTITY=${FINDEST_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null \
    | sed -n 's/.*"\(Apple Development: .*\)"/\1/p' | head -n 1)}
if [ -n "$IDENTITY" ]; then
    codesign --force --deep --sign "$IDENTITY" "$APP"
    echo "Signed with: $IDENTITY"
else
    codesign --force --deep --sign - "$APP"
    echo "Signed ad-hoc (permissions will reset on each rebuild; set FINDEST_SIGN_IDENTITY to fix)"
fi
echo "Built $APP"

if [ "${1:-}" = "--install" ]; then
    osascript -e 'quit app "Findest"' >/dev/null 2>&1 || true
    rm -rf /Applications/Findest.app
    cp -R "$APP" /Applications/
    open /Applications/Findest.app
    echo "Installed and launched /Applications/Findest.app"
else
    echo "Install with: $0 --install"
fi
