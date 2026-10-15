#!/bin/sh
# Regenerates assets/AppIcon.png, assets/Assets.car and assets/AppIcon.icns
# from scripts/make_icon.swift. Needs Xcode 26+ (actool with Icon Composer
# support): macOS 26+ shows an icon full size only when it comes from an
# Assets.car; a plain .icns gets shrunk onto a gray plate. The .icns is the
# fallback for older macOS.
set -eu
cd "$(dirname "$0")/.."
TMP=$(mktemp -d)
swift scripts/make_icon.swift "$TMP/icon.png"
ICON="$TMP/AppIcon.icon"
mkdir -p "$ICON/Assets" "$TMP/out"
cp "$TMP/icon.png" "$ICON/Assets/art.png"
cat > "$ICON/icon.json" <<'JSON'
{
  "fill" : "automatic",
  "groups" : [
    {
      "layers" : [ { "glass" : false, "image-name" : "art.png", "name" : "art" } ],
      "shadow" : { "kind" : "none", "opacity" : 0 },
      "translucency" : { "enabled" : false, "value" : 0 }
    }
  ],
  "supported-platforms" : { "squares" : [ "macOS" ] }
}
JSON
xcrun actool "$ICON" --compile "$TMP/out" --platform macosx --minimum-deployment-target 11.0 \
    --app-icon AppIcon --output-partial-info-plist "$TMP/out/partial.plist" >/dev/null
cp "$TMP/out/Assets.car" assets/Assets.car
cp "$TMP/out/AppIcon.icns" assets/AppIcon.icns
cp "$TMP/icon.png" assets/AppIcon.png
rm -rf "$TMP"
echo "wrote assets/Assets.car, assets/AppIcon.icns"
