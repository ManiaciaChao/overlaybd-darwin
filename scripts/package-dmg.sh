#!/bin/bash
# Developer ID archive -> distribution export -> DMG -> Apple notarization.
# OBD_SKIP_NOTARIZATION=1 is explicitly labeled unnotarized, never a release.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${OBD_DEVELOPMENT_TEAM:?Set OBD_DEVELOPMENT_TEAM to your Apple team ID}"
if [[ "${OBD_SKIP_NOTARIZATION:-0}" != 1 ]]; then
    : "${OBD_NOTARY_PROFILE:?Set OBD_NOTARY_PROFILE to an existing notarytool keychain profile}"
fi
BUILD="$PWD/macos/build"
ARCHIVE="$BUILD/OverlayBD.xcarchive"
EXPORT="$BUILD/distribution"
python3 scripts/generate-xcode-project.py
if [[ "${OBD_SKIP_ARCHIVE:-0}" != 1 ]]; then
    xcodebuild -project macos/OverlayBD.xcodeproj -scheme OverlayBDHost \
        -configuration Release -derivedDataPath "$BUILD/xcode" -archivePath "$ARCHIVE" \
        -allowProvisioningUpdates "DEVELOPMENT_TEAM=$OBD_DEVELOPMENT_TEAM" archive
fi
python3 - "$BUILD/ExportOptions.plist" "$OBD_DEVELOPMENT_TEAM" <<'PY'
import plistlib,sys
with open(sys.argv[1],'wb') as f:
    plistlib.dump({'method':'developer-id','teamID':sys.argv[2], 'signingStyle':'automatic',
                  'destination':'export','manageAppVersionAndBuildNumber':False},f)
PY
xcodebuild -exportArchive -archivePath "$ARCHIVE" -exportPath "$EXPORT" \
    -exportOptionsPlist "$BUILD/ExportOptions.plist" -allowProvisioningUpdates
APP="$EXPORT/OverlayBDHost.app"
# Export must supply a distribution profile for the restricted FSKit entitlement.
# A development profile restricts colleagues to registered machines.
python3 scripts/verify-distribution.py "$APP"
codesign --verify --deep --strict "$APP"
STAGE=$(mktemp -d "$BUILD/dmg-stage.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
# ditto preserves nested extension/profile/signature metadata.
ditto "$APP" "$STAGE/OverlayBD.app"
ln -s /Applications "$STAGE/Applications"
cp macos/DISTRIBUTION.txt "$STAGE/Installation.txt"
VERSION=$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$APP/Contents/Info.plist")
SUFFIX=""
if [[ "${OBD_SKIP_NOTARIZATION:-0}" == 1 ]]; then SUFFIX="-unnotarized"; fi
DMG="$BUILD/OverlayBD-$VERSION-arm64$SUFFIX.dmg"
# ZFS compression makes st_blocks underestimate space needed on the HFS+ DMG.
# Size from logical bytes, leaving filesystem/catalog headroom.
SIZE_MB=$(python3 - "$STAGE" <<'PY'
from pathlib import Path
import sys
size=sum(p.stat().st_size for p in Path(sys.argv[1]).rglob('*') if p.is_file() and not p.is_symlink())
print(max(128, (size*2 + 1024**2-1)//1024**2 + 64))
PY
)
hdiutil create -volname OverlayBD -srcfolder "$STAGE" -size "${SIZE_MB}m" -ov -format UDZO "$DMG"
IDENTITY="${OBD_SIGN_IDENTITY:-Developer ID Application}"
codesign --force --sign "$IDENTITY" --timestamp "$DMG"
if [[ "${OBD_SKIP_NOTARIZATION:-0}" != 1 ]]; then
    xcrun notarytool submit "$DMG" --keychain-profile "$OBD_NOTARY_PROFILE" --wait \
        --output-format json > "$BUILD/notarization.json"
    python3 - "$BUILD/notarization.json" <<'PY'
import json,sys
r=json.load(open(sys.argv[1]))
if r.get('status')!='Accepted':
    raise SystemExit('Notarization not accepted; inspect macos/build/notarization.json and notarytool log')
PY
    xcrun stapler staple "$DMG"
    xcrun stapler validate "$DMG"
    spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG"
fi
hdiutil verify "$DMG"
printf 'Created %s\n' "$DMG"
