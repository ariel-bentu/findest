#!/bin/sh
# Regenerates assets/AppIcon.icns from scripts/make_icon.swift.
set -eu
cd "$(dirname "$0")/.."
TMP=$(mktemp -d)
swift scripts/make_icon.swift "$TMP/icon.png"
SET="$TMP/AppIcon.iconset"
mkdir -p "$SET"
for s in 16 32 128 256 512; do
    sips -z $s $s "$TMP/icon.png" --out "$SET/icon_${s}x${s}.png" >/dev/null
    d=$((s * 2))
    sips -z $d $d "$TMP/icon.png" --out "$SET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$SET" -o assets/AppIcon.icns
cp "$TMP/icon.png" assets/AppIcon.png
rm -rf "$TMP"
echo "wrote assets/AppIcon.icns"
